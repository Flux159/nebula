//! POST /build — unpack the context tar, run slim-build, stream classic
//! builder progress lines.

use crate::engine::EngineRef;
use slim_http::Ctx;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

type R = io::Result<()>;

pub fn handle(engine: &EngineRef, ctx: &mut Ctx) -> R {
    // Query params (docker build sends most config here).
    let tag = ctx.head.query_str("t").map(|s| s.to_string());
    let dockerfile = ctx
        .head
        .query_str("dockerfile")
        .unwrap_or("Dockerfile")
        .to_string();
    let target = ctx
        .head
        .query_str("target")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let nocache = ctx.head.query_bool("nocache");
    let build_args = parse_json_map(ctx.head.query_str("buildargs"));
    let labels = parse_json_map(ctx.head.query_str("labels"));

    // Read + unpack the context to a temp dir.
    let ctx_dir = engine
        .paths
        .run
        .join(format!("build-ctx-{}", slim_net::rand_id()));
    if let Err(e) = std::fs::create_dir_all(&ctx_dir) {
        return ctx.respond_error(500, format!("mkdir build context: {e}"));
    }
    let body = ctx.body_vec(4 * 1024 * 1024 * 1024)?;
    if let Err(e) = unpack_context(&body, &ctx_dir) {
        let _ = std::fs::remove_dir_all(&ctx_dir);
        return ctx.respond_error(400, format!("error reading build context: {e}"));
    }

    let opts = slim_build::BuildOptions {
        dockerfile,
        tag: tag.clone(),
        target,
        build_args,
        no_cache: nocache,
        labels,
    };

    let mut w = ctx.stream(200, "application/json")?;
    let mut emit = |line: &str| {
        let msg = slim_api::ProgressMessage {
            stream: Some(line.to_string()),
            ..Default::default()
        };
        if let Ok(mut b) = serde_json::to_vec(&msg) {
            b.push(b'\n');
            let _ = w.write_all(&b);
        }
    };

    let engine2 = engine.clone();
    let mut ensure = move |reference: &str,
                           prog: &mut slim_build::Progress|
          -> Result<slim_image::ImageRecord, slim_build::BuildError> {
        prog(&format!("Pulling {reference}...\n"));
        engine2
            .ensure_image(reference)
            .map_err(|e| slim_build::BuildError(e.to_string()))
    };

    let result = slim_build::build(&engine.store, &ctx_dir, &opts, &mut ensure, &mut emit);
    let _ = std::fs::remove_dir_all(&ctx_dir);

    match result {
        Ok(rec) => {
            // docker build clients also look for an aux image-id message.
            let aux = slim_api::ProgressMessage {
                aux: Some(serde_json::json!({"ID": rec.id})),
                ..Default::default()
            };
            if let Ok(mut b) = serde_json::to_vec(&aux) {
                b.push(b'\n');
                let _ = w.write_all(&b);
            }
            Ok(())
        }
        Err(e) => {
            // Stream the error in-band (docker shows the errorDetail).
            let msg = slim_api::ProgressMessage::from_error(e.to_string());
            if let Ok(mut b) = serde_json::to_vec(&msg) {
                b.push(b'\n');
                let _ = w.write_all(&b);
            }
            Ok(())
        }
    }
}

fn unpack_context(body: &[u8], dir: &std::path::Path) -> io::Result<()> {
    let reader: Box<dyn Read> = if body.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2_decoder(body))
    } else {
        Box::new(body)
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_overwrite(true);
    ar.unpack(dir)
}

// flate2 isn't a slimd dep; pull gzip via slim-image's re-export path would be
// cleaner, but the daemon already links flate2 transitively. Use a tiny inline
// gunzip using the std-available path: shell out is overkill — instead depend
// on flate2 directly.
fn flate2_decoder(body: &[u8]) -> flate2::read::GzDecoder<&[u8]> {
    flate2::read::GzDecoder::new(body)
}

fn parse_json_map(s: Option<&str>) -> BTreeMap<String, String> {
    s.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}
