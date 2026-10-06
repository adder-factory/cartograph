import type { TurboModule } from 'react-native';
import { TurboModuleRegistry } from 'react-native';

export interface Spec extends TurboModule {
  doSomething(name: string): Promise<string>
  syncName(): string
  readonly constants: object;
}

export default TurboModuleRegistry.getEnforcing<Spec>('RNThing');
