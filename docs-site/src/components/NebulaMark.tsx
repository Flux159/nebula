/**
 * NebulaMark - the Nebula brand mark.
 *
 * This was a hand-drawn SVG of the same shape. It is the app icon now, so the
 * site, the desktop app and the README all show one image rather than three
 * drawings of it that drift apart when only one gets updated.
 *
 * BASE_URL keeps it correct under the /nebula/ project-pages path as well as
 * at a domain root.
 */

export function NebulaMark({ size = 24 }: { size?: number }) {
  return (
    <img
      src={`${import.meta.env.BASE_URL}icon.png`}
      width={size}
      height={size}
      alt=""
      aria-hidden="true"
      style={{ flexShrink: 0, display: 'block' }}
    />
  );
}
