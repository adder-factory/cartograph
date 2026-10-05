<script context="module">
  export const TAG = 'legacy';
  export function describe(name) {
    return `${TAG}:${name}`;
  }
</script>

<script>
  import { onMount, createEventDispatcher } from 'svelte';
  import { save } from './api.js';
  import { page } from '$app/stores';
  import { PUBLIC_MODE } from '$env/static/public';

  export let name = 'anon';
  const dispatch = createEventDispatcher();

  $: greeting = describe(name);

  onMount(() => {
    save(name.length);
    dispatch('ready');
  });
</script>

<h1 title={PUBLIC_MODE}>{greeting} on {$page.url.pathname}</h1>
