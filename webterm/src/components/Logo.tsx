// Inline logo using currentColor so it follows the active theme.

type Props = {
  className?: string;
  size?: number;
};

export default function Logo({ className = '', size = 32 }: Props) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 64 64"
      xmlns="http://www.w3.org/2000/svg"
      fill="none"
      className={className}
    >
      <path
        d="M18 44 C9 44, 9 30, 18 30 C16 19, 31 16, 35 25 C41 17, 54 22, 50 31 C58 31, 58 44, 50 44 Z"
        stroke="currentColor"
        strokeWidth="3"
        strokeLinejoin="round"
        fill="currentColor"
        fillOpacity="0.08"
      />
    </svg>
  );
}
