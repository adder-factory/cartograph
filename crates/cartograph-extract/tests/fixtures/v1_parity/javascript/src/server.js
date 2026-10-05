import { Hono } from 'hono';
import { Command } from 'commander';
import buildModel from './models.js';

function wrap(instance) {
  return instance;
}

const w = wrap(new Hono());
w.get('/wrapped', (c) => c.text('w'));

const app = new Hono();
app.get('/a', (c) => {
  app.get('/b', (inner) => inner.text('b'));
  return c.text('a');
});
const child = new Hono();
child.get('/list/', (c) => c.text('list'));
app.route('/v1', child);

const holder = {};
holder.api = new Hono();
holder.api.get('/ignored', (c) => c.text('no'));

const program = new Command();
const admin = program.command('admin');
admin.command('reset').command('hard');
program
  .command('init [path]')
  .action((dir) => buildModel(dir));
const db = { command: (sql) => sql };
db.command('SELECT * FROM t');

function getQ() {
  return new Response('q');
}

function postQ() {
  return new Response('p');
}

Bun.serve({
  routes: {
    '/q': { 'GET': getQ, "POST": postQ },
    '/d': { description: 'docs', GET: getQ },
    '/healthz': () => new Response('ok'),
  },
});

export default app;
