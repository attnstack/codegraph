import { Checkout } from '@demo/services/checkout';

// 箭头函数具名导出：variable_declarator + arrow_function 抽取路径
export const useCart = () => {
  const checkout = new Checkout();
  return checkout;
};
