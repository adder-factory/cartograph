import 'reflect-metadata';
import defaultFormatter, { User, UserRepo, Builder, TinyCache, MAX_RETRY as RETRIES } from '../shared/models';
import * as models from '../shared/models';
import type { Options, Handler, Role } from '../shared/models';
import {
  formatName,
  shout,
  doubled,
} from '@shared/format';
import { Injectable, Input, Log } from './helpers';
import { track, memo, handlerA, handlerB } from './helpers';
import fs = require('fs');

const lazyHelpers = require('./helpers');

@Injectable('users')
export class UserService {
  @Input() name: string = '';
  repo: UserRepo;
  private cache: TinyCache = new TinyCache();
  protected role: Role = 'user';
  count = 0;
  handler = memo(() => {
    track();
  });
  onClick = (event: Event): void => {
    this.save(event.type);
  };

  constructor(repo: UserRepo) {
    this.repo = repo;
  }

  @Log('save')
  save(label: string): void {
    const user = new User(label);
    this.repo.save(user);
    this.helper();
    this.cache.remember(label);
    console.log(formatName(label, shout(label)));
  }

  helper(): number {
    return doubled(RETRIES);
  }

  static make(): UserService {
    return new UserService(new UserRepo());
  }

  commitAll(): boolean {
    const builder = Builder.create();
    return builder.build().commit();
  }

  async loadLater(opts?: import('../shared/models').Options): Promise<void> {
    const mod = await import('./helpers');
    mod.track();
    void opts;
  }

  describe(user: models.User): string;
  describe(user: models.User): string {
    return defaultFormatter(user.email);
  }
}

export class AdminService extends UserService implements Options {
  verbose = true;

  override save(label: string): void {
    super.save(label.toUpperCase());
  }
}

export function useUserService(): UserService {
  return UserService.make();
}

export function parsePayload<Payload>(raw: string): Payload {
  return JSON.parse(raw) as Payload;
}

export function process(items: string[]): number[] {
  return items.map((value: string) => value.length);
}

export const ROUTES = {
  a: handlerA,
  list: [handlerB],
  handlerA,
};

export const runHandler: Handler<string> = async (input) => {
  let local = input.trim();
  console.log(local);
};

function loadCommonJs(): void {
  require('./helpers');
  const cfg = require(`./helpers`);
  void cfg;
  void fs;
  void lazyHelpers;
}
