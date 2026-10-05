import { NativeModules } from 'react-native';

export function saveAll() {
  NativeModules.Store.save('k');
  return NativeModules.Store.syncGet();
}
