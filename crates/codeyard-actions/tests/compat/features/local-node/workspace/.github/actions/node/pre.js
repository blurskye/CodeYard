const fs = require('fs');
fs.appendFileSync(process.env.GITHUB_STATE, 'token=restored\n');
console.log('compat:node-pre');
