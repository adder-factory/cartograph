<script lang="ts">
  import { formatCount, type CountStyle } from './format';
  import * as api from './api.js';
  import Badge from './Badge.svelte';

  interface Props {
    start?: number;
    style?: CountStyle;
  }

  enum Step {
    One = 1,
    Two = 2,
  }

  let { start = 0, style = 'plain' }: Props = $props();
  let count = $state(start);
  let history = $state.raw<number[]>([]);
  const doubled = $derived(count * 2);
  const label = $derived.by(() => formatCount(count, style));

  $effect.pre(() => {
    history = [...history, count];
  });

  class Ticker {
    ticks = 0;
    tick(): number {
      this.ticks += Step.One;
      return this.ticks;
    }
  }

  const ticker = new Ticker();

  function increment(): void {
    count += Step.Two;
    ticker.tick();
    api.save(count);
  }

  const reset = () => {
    count = start;
  };
</script>

<button on:click={increment}>{label}</button>
<p>{doubled} / {api.fmt(count)}</p>
{#if count > 10}
  <Badge text={formatCount(count, 'plain')} />
{:else}
  <span>{reset}</span>
{/if}
{#each history as entry}
  <li>{entry}</li>
{/each}

<style>
  button { color: teal; }
</style>
