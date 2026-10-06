/**
 * @typedef {import('./types').Point} Point
 */

/**
 * Distance between two points.
 * @param {Point} a
 * @param {Point} b
 * @returns {number}
 */
export function distance(a, b) {
  return Math.hypot(a.x - b.x, a.y - b.y);
}

/**
 * @param {Point[]} points
 * @param {(p: Point) => void} visit
 */
export function each(points, visit) {
  points.forEach(visit);
  return distance(points[0], points[1]);
}

/** @type {Point} */
export const ORIGIN = { x: 0, y: 0 };
