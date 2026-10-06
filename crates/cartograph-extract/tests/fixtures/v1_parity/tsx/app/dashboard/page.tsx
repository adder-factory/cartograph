import { Star } from '../../src/components/icons';

export const metadata = { title: 'Dashboard' };

export default async function DashboardPage() {
  const stats = await fetchStats();
  return <section><Star size={stats.length} /></section>;
}

async function fetchStats(): Promise<number[]> {
  return [1, 2, 3];
}
