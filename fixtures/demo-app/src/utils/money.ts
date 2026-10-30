export function format(amount: number): string {
  return `$${amount.toFixed(2)}`;
}
