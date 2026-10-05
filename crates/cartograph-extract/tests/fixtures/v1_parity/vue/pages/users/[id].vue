<template>
  <UserCard :user="user" />
</template>

<script setup lang="ts">
import UserCard from '~/components/UserCard.vue';
import { findUser } from '../../server/api/users';
import { useHead } from '#imports';

definePageMeta({ middleware: 'auth' });
const route = useRoute();
const user = findUser(String(route.params.id));
useHead({ title: user.first });
</script>
