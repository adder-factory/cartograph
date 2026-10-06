export interface Post {
  title: string;
  date: Date;
}

export function formatDate(date: Date): string {
  return date.toISOString().slice(0, 10);
}

export async function getPosts(): Promise<Post[]> {
  return [{ title: 'Hello', date: new Date(0) }];
}
