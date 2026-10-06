export function formatName(first: string, last: string): string {
  return `${first} ${last}`;
}

export const shout = (text: string): string => text.toUpperCase();

function orig(value: number): number {
  return value * 2;
}

export { orig as doubled };
export { default as fmt } from './models';
export * from './models';
