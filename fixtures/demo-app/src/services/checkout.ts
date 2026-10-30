import { Order } from '../models/order';
import * as payment from './payment';

export class Checkout {
  createOrder(id: string, amount: number): string {
    const order = new Order(id, amount);
    return payment.charge(order.total());
  }

  validate(): boolean {
    return true;
  }

  process(callback: () => void): void {
    callback();
  }
}
