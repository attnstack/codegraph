import { Checkout } from '../src/index';

describe('Checkout', () => {
  it('validates orders', () => {
    const c = new Checkout();
    expect(c.validate()).toBe(true);
  });
});
