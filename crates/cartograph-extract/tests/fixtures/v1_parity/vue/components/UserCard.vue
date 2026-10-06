<template>
  <section class="user-card">
    <h2>{{ formatName(user.first, user.last) }}</h2>
    <p v-if="visible">{{ status.toUpperCase() }}</p>
    <Panel :title="user.first" @close="hide" />
    <BaseBadge v-for="tag in tags" :key="tag" :label="tag" />
  </section>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue';
import Panel from '~/components/Panel';
import BaseBadge from './BaseBadge.vue';
import {
  formatName,
  type UserShape,
} from '../server/api/users';
import * as api from '../server/api/users';

interface Props {
  user: UserShape;
  tags?: string[];
}

enum Status {
  Active = 'active',
  Hidden = 'hidden',
}

const props = withDefaults(defineProps<Props>(), { tags: () => [] });
const emit = defineEmits<{ (e: 'hide'): void }>();
const visible = ref(true);
const status = computed(() => (visible.value ? Status.Active : Status.Hidden));
const { data } = await useFetch('/api/users');
const route = useRoute();

class Tracker {
  private hits = 0;
  hit(): number {
    this.hits += 1;
    return this.hits;
  }
}

const tracker = new Tracker();

function hide(): void {
  visible.value = false;
  tracker.hit();
  emit('hide');
  api.listUsers();
}

const reload = async () => {
  await navigateTo(`/users/${route.params.id}`);
};

defineExpose({ hide, reload, data, props });
</script>

<style scoped>
.user-card { padding: 4px; }
</style>
