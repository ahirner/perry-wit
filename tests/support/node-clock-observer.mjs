import timers from 'node:timers/promises';
import {syncBuiltinESMExports} from 'node:module';
const trace = [];
const originalTimeout = timers.setTimeout;
timers.setTimeout = async (milliseconds, ...arguments_) => {
    const delay = milliseconds >= 1 && milliseconds <= 2147483647 ? Math.trunc(milliseconds) : 1;
    trace.push(`wait:${delay * 1000000}`);
    const result = await originalTimeout(milliseconds, ...arguments_);
    trace.push('resume');
    return result;
};
syncBuiltinESMExports();
let randomCalls = 0;
Math.random = () => { trace.push('random'); return ++randomCalls / 8; };
