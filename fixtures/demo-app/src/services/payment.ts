import { format } from '@demo/utils/money';
import { log } from '../utils/logger';

export function charge(amount: number): string {
  log(`charging ${amount}`);
  return format(amount);
}

export function refund(amount: number): string {
  return format(-amount);
}
