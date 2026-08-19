const test = require('node:test');
const assert = require('node:assert');
const policy = require('../source/main.js');

test('always returns the fixed nonsense shape regardless of input', () => {
  assert.deepStrictEqual(policy({ cells: [{ id: 'a', qps: 999 }] }), { nonsense: 1 });
  assert.deepStrictEqual(policy(undefined), { nonsense: 1 });
  assert.deepStrictEqual(policy({}), { nonsense: 1 });
});
