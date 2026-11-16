export function greet(name) {
  return `hi ${name || 'there'}`;
}

console.log(greet("loom"));
console.log(greet(""));
