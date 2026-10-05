export function Header({ text }) {
  return <h1>{text}</h1>;
}

export const Badge = ({ count }) => <b>{count}</b>;

export function formatTitle(title) {
  return title.toUpperCase();
}

export default class Base {
  render() {
    return null;
  }
}

export const Mixin = {
  Inner: class Inner {},
};

export const ROUTE_TABLE = {
  header: Header,
  badge: Badge,
};
