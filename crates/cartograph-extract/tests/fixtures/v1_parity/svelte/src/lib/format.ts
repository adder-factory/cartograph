export type CountStyle = 'plain' | 'fancy';

export function formatCount(value: number, style: CountStyle): string {
  return style === 'fancy' ? `#${value}` : String(value);
}
