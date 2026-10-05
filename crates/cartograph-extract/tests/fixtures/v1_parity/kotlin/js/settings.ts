import { requireNativeModule } from 'expo-modules-core';

const ExpoSettings = requireNativeModule('ExpoSettings');

export async function applyTheme(): Promise<string> {
  ExpoSettings.getTheme();
  ExpoSettings.spaced();
  return ExpoSettings.setTheme('light');
}
