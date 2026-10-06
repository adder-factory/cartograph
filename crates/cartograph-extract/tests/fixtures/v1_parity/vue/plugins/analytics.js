export function trackView(name) {
  return { name };
}

export default defineNuxtPlugin(() => {
  trackView('boot');
});
