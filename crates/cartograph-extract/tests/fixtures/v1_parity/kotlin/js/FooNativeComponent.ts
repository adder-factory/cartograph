import { codegenNativeComponent } from 'react-native';
import type { ViewProps } from 'react-native';

export interface NativeProps extends ViewProps {
  color?: number;
  enabled?: boolean;
}

export default codegenNativeComponent<NativeProps>('Foo');
