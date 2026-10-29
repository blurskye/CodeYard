const fs = require('fs');
if (process.env.INPUT_MESSAGE !== 'hello') throw Error('missing input');
fs.appendFileSync(process.env.GITHUB_OUTPUT, 'answer=42\n');
console.log('compat:node-main');
