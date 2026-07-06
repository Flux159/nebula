/**
 * NebulaMark - the nebula brand mark: layered translucent gas clouds with a
 * bright core, in cyan/indigo.
 */

export function NebulaMark({ size = 24 }: { size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 64 64"
      aria-hidden="true"
      style={{ flexShrink: 0 }}
    >
      <defs>
        <radialGradient id="nebula-core" cx="50%" cy="50%" r="55%">
          <stop offset="0%" stopColor="#f0fdff" />
          <stop offset="35%" stopColor="#a5f3fc" />
          <stop offset="100%" stopColor="#22d3ee" stopOpacity="0" />
        </radialGradient>
        <linearGradient id="nebula-g1" x1="0%" y1="0%" x2="100%" y2="100%">
          <stop offset="0%" stopColor="#22d3ee" />
          <stop offset="100%" stopColor="#6366f1" />
        </linearGradient>
        <linearGradient id="nebula-g2" x1="100%" y1="100%" x2="0%" y2="0%">
          <stop offset="0%" stopColor="#818cf8" />
          <stop offset="100%" stopColor="#0891b2" />
        </linearGradient>
      </defs>
      <ellipse cx="32" cy="32" rx="27" ry="17" fill="url(#nebula-g1)" opacity="0.42" transform="rotate(-28 32 32)" />
      <ellipse cx="32" cy="32" rx="26" ry="15" fill="url(#nebula-g2)" opacity="0.45" transform="rotate(34 32 32)" />
      <ellipse cx="32" cy="32" rx="22" ry="13" fill="url(#nebula-g1)" opacity="0.5" transform="rotate(85 32 32)" />
      <circle cx="32" cy="32" r="14" fill="url(#nebula-core)" />
      <circle cx="32" cy="32" r="4.5" fill="#f0fdff" />
      <path d="M46 15 l1 2.2 2.2 1 -2.2 1 -1 2.2 -1 -2.2 -2.2 -1 2.2 -1 Z" fill="#cffafe" />
      <circle cx="16" cy="45" r="1.1" fill="#e0f2fe" />
      <circle cx="49" cy="42" r="0.8" fill="#cffafe" opacity="0.85" />
      <circle cx="15" cy="20" r="0.7" fill="#e0f2fe" opacity="0.7" />
    </svg>
  );
}
