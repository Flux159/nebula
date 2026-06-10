//! Minimal DNS wire codec for Nebula's resolver path.
//!
//! Guest queries hit the agent's 127.0.0.1:53 proxy, hop to nebulad over UDP,
//! and get resolved on the host with getaddrinfo — so DNS follows whatever the
//! Mac's resolver does (VPN split-horizon included). Only what that path needs
//! is implemented: parse one question, answer with A/AAAA, or signal failure.

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
}

pub const QTYPE_A: u16 = 1;
pub const QTYPE_AAAA: u16 = 28;

const FLAG_QR: u16 = 0x8000;
const FLAG_RD: u16 = 0x0100;
const FLAG_RA: u16 = 0x0080;
const RCODE_SERVFAIL: u16 = 2;
const RCODE_NXDOMAIN: u16 = 3;

/// Parse the first question of a DNS query. Returns (id, question).
pub fn parse_query(packet: &[u8]) -> Option<(u16, Question)> {
    if packet.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([packet[0], packet[1]]);
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount == 0 {
        return None;
    }
    let mut pos = 12;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *packet.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        // Compression pointers are illegal in a question we originate.
        if len & 0xC0 != 0 {
            return None;
        }
        let label = packet.get(pos + 1..pos + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
        if labels.len() > 32 {
            return None;
        }
    }
    let qtype = u16::from_be_bytes([*packet.get(pos)?, *packet.get(pos + 1)?]);
    let qclass = u16::from_be_bytes([*packet.get(pos + 2)?, *packet.get(pos + 3)?]);
    Some((
        id,
        Question {
            name: labels.join("."),
            qtype,
            qclass,
        },
    ))
}

fn encode_name(name: &str, out: &mut Vec<u8>) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        let bytes = label.as_bytes();
        out.push(bytes.len().min(63) as u8);
        out.extend_from_slice(&bytes[..bytes.len().min(63)]);
    }
    out.push(0);
}

fn response_header(id: u16, rcode: u16, ancount: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&(FLAG_QR | FLAG_RD | FLAG_RA | rcode).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
}

fn push_question(q: &Question, out: &mut Vec<u8>) {
    encode_name(&q.name, out);
    out.extend_from_slice(&q.qtype.to_be_bytes());
    out.extend_from_slice(&q.qclass.to_be_bytes());
}

/// Build a response carrying the given IPs (filtered to the question's type).
pub fn build_response(id: u16, q: &Question, ips: &[std::net::IpAddr], ttl: u32) -> Vec<u8> {
    let answers: Vec<&std::net::IpAddr> = ips
        .iter()
        .filter(|ip| match q.qtype {
            QTYPE_A => ip.is_ipv4(),
            QTYPE_AAAA => ip.is_ipv6(),
            _ => false,
        })
        .collect();

    let mut out = Vec::with_capacity(128);
    if answers.is_empty() {
        // No data of the requested type: NOERROR with zero answers.
        response_header(id, 0, 0, &mut out);
        push_question(q, &mut out);
        return out;
    }
    response_header(id, 0, answers.len() as u16, &mut out);
    push_question(q, &mut out);
    for ip in answers {
        // Name compression: pointer to offset 12 (the question name).
        out.extend_from_slice(&[0xC0, 0x0C]);
        match ip {
            std::net::IpAddr::V4(v4) => {
                out.extend_from_slice(&QTYPE_A.to_be_bytes());
                out.extend_from_slice(&1u16.to_be_bytes());
                out.extend_from_slice(&ttl.to_be_bytes());
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                out.extend_from_slice(&QTYPE_AAAA.to_be_bytes());
                out.extend_from_slice(&1u16.to_be_bytes());
                out.extend_from_slice(&ttl.to_be_bytes());
                out.extend_from_slice(&16u16.to_be_bytes());
                out.extend_from_slice(&v6.octets());
            }
        }
    }
    out
}

pub fn build_error(id: u16, q: &Question, nxdomain: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    response_header(
        id,
        if nxdomain {
            RCODE_NXDOMAIN
        } else {
            RCODE_SERVFAIL
        },
        0,
        &mut out,
    );
    push_question(q, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut p = vec![0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        encode_name(name, &mut p);
        p.extend_from_slice(&qtype.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p
    }

    #[test]
    fn parses_query() {
        let p = query("registry-1.docker.io", QTYPE_A);
        let (id, q) = parse_query(&p).unwrap();
        assert_eq!(id, 0xABCD);
        assert_eq!(q.name, "registry-1.docker.io");
        assert_eq!(q.qtype, QTYPE_A);
    }

    #[test]
    fn builds_a_response_parseable_by_us() {
        let p = query("example.com", QTYPE_A);
        let (id, q) = parse_query(&p).unwrap();
        let resp = build_response(id, &q, &["93.184.216.34".parse().unwrap()], 60);
        assert_eq!(&resp[0..2], &id.to_be_bytes());
        assert_eq!(resp[2] & 0x80, 0x80); // QR set
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1); // ANCOUNT
        assert_eq!(&resp[resp.len() - 4..], &[93, 184, 216, 34]);
    }

    #[test]
    fn aaaa_question_filters_v4_answers() {
        let p = query("example.com", QTYPE_AAAA);
        let (id, q) = parse_query(&p).unwrap();
        let resp = build_response(id, &q, &["93.184.216.34".parse().unwrap()], 60);
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0); // no answers
        assert_eq!(resp[3] & 0x0F, 0); // NOERROR
    }

    #[test]
    fn error_response_sets_rcode() {
        let p = query("nope.invalid", QTYPE_A);
        let (id, q) = parse_query(&p).unwrap();
        let resp = build_error(id, &q, true);
        assert_eq!(resp[3] & 0x0F, 3); // NXDOMAIN
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_query(&[0u8; 5]).is_none());
        assert!(parse_query(&[]).is_none());
    }
}
