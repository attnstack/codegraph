import { Button } from './Button';

// JSX 组件调用：`<Button />` 与 Button(props) 在调用图上等价
export function App(): string {
  return <Button label="hello" />;
}
