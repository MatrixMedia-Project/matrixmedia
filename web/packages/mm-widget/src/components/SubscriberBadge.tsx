/** Map tier name (lowercase) to hex color, reused from DonationOverlay. */
const TIER_COLORS: Record<string, string> = {
  blue: '#1E88E5',
  green: '#43A047',
  yellow: '#FDD835',
  orange: '#FB8C00',
  magenta: '#E91E63',
  red: '#E53935',
  gold: '#FFD700',
  silver: '#B0BEC5',
  bronze: '#CD7F32',
};

/** Pick a text color (dark or light) based on background brightness. */
function textColorFor(hex: string): string {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  const luminance = 0.299 * r + 0.587 * g + 0.114 * b;
  return luminance > 160 ? '#1a1a2e' : '#ffffff';
}

interface SubscriberBadgeProps {
  tierName: string;
}

/**
 * A small colored badge rendered next to a username to indicate
 * the viewer's subscription tier. The background color is chosen
 * based on the tier name, falling back to a neutral blue.
 */
export function SubscriberBadge(props: SubscriberBadgeProps) {
  const bg = () => TIER_COLORS[props.tierName.toLowerCase()] ?? TIER_COLORS.blue;
  const fg = () => textColorFor(bg());

  return (
    <span
      class="mm-subscriber-badge"
      style={{
        'background-color': bg(),
        color: fg(),
      }}
    >
      {props.tierName}
    </span>
  );
}
