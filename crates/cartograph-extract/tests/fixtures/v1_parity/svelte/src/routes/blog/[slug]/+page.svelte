<script lang="ts">
  import Counter from '$lib/Counter.svelte';
  import Legacy, { describe } from '../../../lib/Legacy.svelte';
  let { data } = $props();
</script>

<Counter start={data.count} />
<Legacy name={describe(data.slug)} />
