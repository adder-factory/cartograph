export function Injectable(key?: string): ClassDecorator {
  return () => undefined;
}

export function Input(): PropertyDecorator {
  return () => undefined;
}

export function Log(label: string): MethodDecorator {
  return () => undefined;
}

export function track(): void {}

export function memo<T>(fn: T): T {
  return fn;
}

export function handlerA(): string {
  return 'a';
}

export function handlerB(): string {
  return 'b';
}

export function authMiddleware(): void {}
