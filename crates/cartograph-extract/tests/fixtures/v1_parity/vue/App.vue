<template>
  <NuxtLayout>
    <NuxtPage />
    <Panel title="root" />
  </NuxtLayout>
</template>

<script setup>
import Panel from './components/Panel/index.vue';
const config = useRuntimeConfig();
</script>
