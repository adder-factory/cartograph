export interface UserShape {
  first: string;
  last: string;
}

const USERS: UserShape[] = [{ first: 'Ada', last: 'Lovelace' }];

export function formatName(first: string, last: string): string {
  return `${first} ${last}`;
}

export function listUsers(): UserShape[] {
  return USERS;
}

export function findUser(id: string): UserShape {
  return USERS[Number(id)] ?? USERS[0];
}

export default defineEventHandler(() => listUsers());
