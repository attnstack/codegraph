export class Order {
  constructor(public id: string, public amount: number) {}

  total(): number {
    return this.amount;
  }
}
