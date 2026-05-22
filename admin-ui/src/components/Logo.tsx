// Inline SVG so currentColor inherits from the surrounding text
// colour — that's the only way to make the logo track the admin
// UI's light / dark theme without shipping two assets. The
// standalone public/logo.svg + public/favicon.svg files exist for
// linking from elsewhere (browser tab, README, etc).
export function Logo({ className = 'h-6 w-6' }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 64 64"
      fill="none"
      className={className}
      aria-hidden="true"
    >
      <path
        d="M18 44 C9 44, 9 30, 18 30 C16 19, 31 16, 35 25 C41 17, 54 22, 50 31 C58 31, 58 44, 50 44 Z"
        stroke="currentColor"
        strokeWidth={3}
        strokeLinejoin="round"
        fill="currentColor"
        fillOpacity={0.08}
      />
    </svg>
  );
}
