import '../styles/base.css';
import styles from './Button.module.css';

export function buttonClass(primary) {
  return primary ? styles.root : styles.label;
}
