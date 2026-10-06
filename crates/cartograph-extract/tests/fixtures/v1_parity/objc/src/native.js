import { NativeModules, NativeEventEmitter } from 'react-native';
import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent';

const { RNThing, Geolocation } = NativeModules;

export const FooView = codegenNativeComponent('Foo');

export async function loadThing() {
  const value = await RNThing.doSomething('a');
  const other = await NativeModules.RNThing.getThing();
  RNThing.spacedRun('b');
  Geolocation.getCurrentPosition(() => {});
  const emitter = new NativeEventEmitter(RNThing);
  emitter.addListener('thingChanged', onThingChanged);
  return [value, other, RNThing.syncName()];
}

function onThingChanged(event) {
  return event;
}
