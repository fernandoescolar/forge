import { useState } from 'react';
import { forge, View, Button, Text } from '@forge-ide/api';

function Counter() {
  const [n, setN] = useState(0);
  return (
    <View style={{ direction: 'column', gap: 8 }}>
      <Text>Counter</Text>
      <Button label={`Clicked ${n} times`} onClick={() => setN(n + 1)} />
    </View>
  );
}

export function activate() {
  forge.panels.register({ id: 'counter', title: 'Counter', render: () => <Counter /> });
}
