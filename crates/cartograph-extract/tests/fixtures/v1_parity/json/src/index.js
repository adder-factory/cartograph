import settings from '../config/settings.json' with { type: 'json' };
import en from '../locales/en.json';

export function serviceName() {
  return settings.service.name;
}

export function firstGreeting() {
  return en[0].text;
}
