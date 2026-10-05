import { EventEmitter } from 'events';
import defaultLogger, { log as writeLog, LEVELS } from './logger.js';
import * as path from 'path';
import './polyfills.js';

const MAX_ITEMS = 50;
let counter = 0;
var legacy = true;

export class Base {
  constructor(name) {
    this.name = name;
  }

  describe() {
    return `${this.name}`;
  }
}

export class Model extends Base {
  static registry = new Map();
  items = [];
  onChange = (event) => {
    this.notify(event);
  };
  onScroll = throttle((event) => {
    this.notify(event);
  }, 100);
  #hiddenCount = 0;

  static create(name) {
    const model = new Model(name);
    Model.registry.set(name, model);
    return model;
  }

  get size() {
    return this.items.length;
  }

  add(item) {
    if (this.items.length >= MAX_ITEMS) {
      writeLog('full');
      return false;
    }
    this.items.push(item);
    this.notify(item);
    return true;
  }

  notify(payload) {
    counter += 1;
    defaultLogger.info(payload);
  }

  *entries() {
    yield* this.items;
  }

  async save() {
    const store = await import('./store.js', { with: { type: 'json' } });
    return store.default;
  }
}

export class Plugin extends path.Base {}

export class Emitter extends EventEmitter {
  constructor() {
    super();
    this.on('ready', this.onReady);
    this.once('close', () => this.onReady());
  }

  onReady() {
    return LEVELS.length;
  }
}

export function throttle(fn, wait) {
  let last = 0;
  return (...args) => {
    const now = wait + last;
    last = now;
    fn(...args);
  };
}

export const helpers = {
  create: Model.create,
  throttle,
};

export default function buildModel(name) {
  const model = Model.create(name);
  model.add({ id: 1 });
  return model;
}

function loadLazily() {
  require('./store.js');
  const cfg = require(`./config.js`);
  return cfg;
}

const fmt = function format(value) {
  return String(value);
};

module.exports = { Model, buildModel, loadLazily, fmt };
