import express from 'express';
import { Hono } from 'hono';
import { Command } from 'commander';
import { authMiddleware, handlerA, handlerB } from '../services/helpers';
import { useUserService } from '../services/user-service';
import { coreFn } from '@acme/core';
import { formatName } from '~/shared/format';

const app = express();
const router = express.Router();

function listUsers(): void {
  useUserService().save('list');
}

function login(): void {
  coreFn(formatName('a', 'b'));
}

function serveStatic(): void {}

router.get('/users', listUsers);
app.post('/login', authMiddleware, login);
app.use('/static', serveStatic);
app.put('/users/:id', handlerA);
app.delete('/users/:id', handlerB);

const api = new Hono();
api.get('/health', (c) => c.text('ok'));
api.post('/items', (c) => c.json({ created: true }));
const root = new Hono();
root.route('/api', api);

const program = new Command();
program
  .command('serve <port>')
  .option('-p, --port <port>')
  .action(() => listUsers());
program.command('stop').action(() => login());

export const server = Bun.serve({
  port: 3000,
  routes: {
    '/healthz': () => new Response('ok'),
    '/items': {
      GET: () => new Response('list'),
      'POST': () => new Response('created'),
    },
  },
  fetch() {
    return new Response('fallback');
  },
});

export default app;
