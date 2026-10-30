import { Checkout } from './checkout';
import { charge } from './payment';

export class RefundService extends Checkout {
  validate(): boolean {
    return false;
  }

  processRefund(amount: number): string {
    this.process(() => undefined);
    return charge(amount);
  }
}
