import { formatCount } from '../../../lib/format';

export function load({ params }: { params: { slug: string } }) {
  return { slug: params.slug, count: formatCount(1, 'plain').length };
}
