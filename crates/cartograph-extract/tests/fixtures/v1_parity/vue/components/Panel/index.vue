<template>
  <div class="panel">
    <header>{{ heading() }}</header>
    <button @click="close">x</button>
    <slot />
  </div>
</template>

<script>
import { defineComponent } from 'vue';
import { listUsers } from '../../server/api/users';

export default defineComponent({
  name: 'Panel',
  props: { title: String },
  emits: ['close'],
  data() {
    return { open: true };
  },
  methods: {
    close() {
      this.open = false;
      this.$emit('close');
    },
    heading() {
      return this.title || 'Panel';
    },
  },
  mounted() {
    listUsers();
  },
});
</script>
