// 不 import handle：裸名调用命中仓库里两个同名导出 → 必须判 ambiguous，不猜
export function dispatchAction(): void {
  handle();
}
