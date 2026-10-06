import React, { useState, useContext, createContext } from 'react';
import type { ReactNode } from 'react';
import * as Icons from './icons';

export interface ButtonProps {
  label: string;
  onPress?: () => void;
  children?: ReactNode;
}

export type Variant = 'primary' | 'ghost';

export const ThemeContext = createContext<Variant>('primary');

export function useToggle(initial: boolean): [boolean, () => void] {
  const [value, setValue] = useState(initial);
  const toggle = () => setValue(!value);
  return [value, toggle];
}

export function Button({ label, onPress, children }: ButtonProps) {
  const variant = useContext(ThemeContext);
  const [open, toggle] = useToggle(false);
  return (
    <button className={variant} onClick={onPress ?? toggle}>
      <Icons.Star size={12} />
      <Label text={label} />
      {open && children}
    </button>
  );
}

export const Label = ({ text }: { text: string }) => <span>{formatLabel(text)}</span>;

function formatLabel(text: string): string {
  return text.trim();
}

export default function IconButton(props: ButtonProps) {
  return <Button {...props} />;
}

export class Counter extends React.Component<ButtonProps, { count: number }> {
  state = { count: 0 };
  private step: number = 1;

  increment = (): void => {
    this.setState({ count: this.state.count + this.step });
  };

  render() {
    return <Button label={String(this.state.count)} onPress={this.increment} />;
  }
}

export enum Size {
  Small = 'sm',
  Large = 'lg',
}
