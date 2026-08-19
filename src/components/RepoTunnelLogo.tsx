type RepoTunnelLogoProps = {
  size?: number;
  className?: string;
};

function RepoTunnelLogo({ size = 38, className = "" }: RepoTunnelLogoProps) {
  return (
    <span
      className={`repotunnel-logo ${className}`.trim()}
      style={{ width: size, height: size }}
      aria-hidden="true"
    >
      <svg viewBox="0 0 48 48" role="img">
        <path d="M15 15.5 23.5 24 15 32.5" />
        <path d="M27 33h8" />
      </svg>
    </span>
  );
}

export default RepoTunnelLogo;
