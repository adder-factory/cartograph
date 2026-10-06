export const PI_APPROX = 3.14;
let counter = 0;

export enum Color {
  Red,
  Green = 2,
  Blue = 'blue'
}

export type Formatter = (value: number) => string;

export interface Logger {
  log(message: string): void;
}

export function helper(id: number): string {
  counter += 1;
  return formatId(id);
}

function formatId(id: number): string {
  return `#${id}`;
}

export const arrow = (x: number): number => x * 2;

const anon = function (y: number): number {
  return y + 1;
};

export class ConsoleLogger implements Logger {
  log(message: string): void {
    console.info(message);
  }
}
