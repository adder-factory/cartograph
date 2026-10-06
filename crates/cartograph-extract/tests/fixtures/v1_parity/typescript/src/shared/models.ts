import { z } from 'zod';

export const MAX_RETRY = 3;
export let retryCount = 0;
var legacyFlag = false;

export interface Entity {
  id: string;
}

export interface Auditable extends Entity {
  createdAt: Date;
  audit(by: string): void;
}

export interface Repo<T extends Entity> {
  find(id: string): T | undefined;
  save(item: T): void;
}

export interface Service<N extends string, Q, R> {
  name: N;
  call(q: Q): R;
}

export interface Req { q: string }
export interface Resp { ok: boolean }

export type Role = 'admin' | 'user';
export type Units = 'metric' | 'imperial';
export type Handler<T> = (input: T) => Promise<void>;
export type MyServiceList = [
  Service<'query_apply_record', Req, Resp>,
  Service<'apply_confirm', Req, Resp>,
];

export enum Status {
  Active,
  Inactive = 'inactive',
  Archived = 2,
}

export const enum Direction {
  Up = 1,
  Down,
}

export namespace Validation {
  export function isEmail(value: string): boolean {
    return value.includes('@');
  }
}

export abstract class BaseRepo<T extends Entity> implements Repo<T> {
  protected items: Map<string, T> = new Map();
  static instances = 0;

  abstract describe(): string;

  find(id: string): T | undefined {
    return this.items.get(id);
  }

  save(item: T): void {
    this.items.set(item.id, item);
    this.touch();
  }

  private touch(): void {
    BaseRepo.instances += 1;
  }
}

export class User implements Auditable {
  id = '';
  createdAt: Date = new Date();
  role: Role = 'user';
  status: Status = Status.Active;

  constructor(public readonly email: string, private nickname?: string) {}

  audit(by: string): void {
    console.log(by);
  }

  get display(): string {
    return this.email;
  }
}

export class UserRepo extends BaseRepo<User> {
  describe(): string {
    return 'users';
  }
}

export class Committer {
  commit(): boolean {
    return true;
  }
}

export class Builder {
  static create(): Builder {
    return new Builder();
  }
  build(): Committer {
    return new Committer();
  }
}

export class TinyCache {
  remember(key: string): string {
    return key;
  }
}

export const UserSchema = z
  .object({
    name: z.string(),
    role: z.enum(['admin', 'user']),
    address: z.object({
      city: z.string(),
      zip: z.string().optional(),
    }),
  })
  .strict();

export type UserInput = z.infer<typeof UserSchema>;

export function readSchemaName(): unknown {
  return UserSchema.shape.name;
}

export function withRetry(task: () => void): number {
  let attempts = 0;
  while (attempts < MAX_RETRY) {
    task();
    attempts += 1;
  }
  return attempts;
}

export function* idGenerator(): Generator<number> {
  let next = 0;
  while (true) yield next++;
}

export async function loadUser(id: string): Promise<User> {
  const user = new User(id);
  return user;
}

export interface Options {
  verbose?: boolean;
}

export default function defaultFormatter(value: string): string {
  return value.trim();
}
