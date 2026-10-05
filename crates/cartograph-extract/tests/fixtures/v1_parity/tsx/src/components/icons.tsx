export interface IconProps {
  size: number;
}

export function Star({ size }: IconProps) {
  return <svg width={size} height={size} />;
}

export const Heart = ({ size }: IconProps): JSX.Element => <svg width={size} />;
