import type { ViewProps } from 'react-native';
import codegenNativeComponent from 'react-native/Libraries/Utilities/codegenNativeComponent';

export interface NativeProps extends ViewProps {
  tint?: string;
  size: number;
  onPress?: () => void;
}

export default codegenNativeComponent<NativeProps>('Bar');
