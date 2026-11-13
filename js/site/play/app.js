import { compileRequest, decode, directory, encode, packagePrograms, readFolder, worldNames } from './workspace.js';
import '../highlight.js';

const $ = id => document.getElementById(id);
let workspace;
let snapshot;
let program;
let operation;
let selectedTab = 'result';
let artifact;
let selectedFunction = '';
let failed = false;
const savedArguments = new Map();
const views = { result: 'Compile & run to see the result.', wat: '', interface: '', log: '' };

function status(message, state = 'ready') {
  $('status').textContent = message;
  $('status').dataset.state = state;
}
function selectOptions(element, values, selected) {
  element.replaceChildren(...values.map(value => new Option(value, value)));
  if (values.includes(selected)) element.value = selected;
}
function functionHelp() {
  const input = program?.functions[$('function').value || program.export];
  $('args-help').textContent = input?.hint || 'A JSON array with one value per parameter, such as ["text", 21]. Use [] for none and strings for 64-bit integers.';
}
function configureProgram(value) {
  program = value;
  savedArguments.clear();
  for (const [name, input] of Object.entries(program?.functions || {})) savedArguments.set(name, JSON.stringify(input.urls ? input.urls.map(path => new URL(path, location.href).href) : input.args || [], null, 2));
  setArguments(savedArguments.get(program?.export) || '[]');
  $('folder-description').textContent = program?.description || `${workspace.size} files loaded. Edits stay in memory.`;
}
function selectExports(names, selected = $('function').value || program?.export) {
  const element = $('function');
  selectOptions(element, names, selected);
  element.disabled = names.length === 0;
  if (!names.includes(selected) && names.length !== 1) {
    element.prepend(new Option(names.length ? 'Choose a function' : program?.export ? `Default: ${program.export} (compile to verify)` : 'Compile to discover functions', '', true, true));
    element.options[0].disabled = true;
  }
  selectedFunction = element.value;
  functionHelp();
}
function showOutput() {
  $('output').textContent = views[selectedTab] || 'Compile a component to inspect this view.';
  $('output').dataset.state = failed && selectedTab === 'result' ? 'error' : '';
}
function selectTab(name) {
  selectedTab = name;
  for (const button of document.querySelectorAll('[data-tab]')) {
    button.setAttribute('aria-selected', String(button.dataset.tab === name));
    button.tabIndex = button.dataset.tab === name ? 0 : -1;
  }
  $('output-panel').setAttribute('aria-labelledby', `tab-${name}`);
  showOutput();
}
function refreshWorlds(preferred = $('world').value) {
  const root = directory($('wit-file').value);
  const sources = [...workspace].filter(([name]) => name.endsWith('.wit') && directory(name) === root).map(([, bytes]) => decode(bytes));
  const names = worldNames(sources.join('\n'));
  selectOptions($('world'), names, preferred);
  if (names.length > 1 && !names.includes(preferred)) {
    $('world').prepend(new Option('Choose a world…', '', true, true));
    $('world').options[0].disabled = true;
  }
  $('world-picker').hidden = names.length < 2;
  $('world-help').hidden = names.length < 2;
  $('world-description').textContent = names.length === 1 ? `World: ${names[0]}` : names.length ? '' : 'Add a WIT world to describe this component’s interface.';
}
function setEditor(id, bytes) {
  const editor = $(id);
  editor.value = decode(bytes);
  editor.setSelectionRange(0, 0);
  editor.scrollTop = editor.scrollLeft = 0;
  editor.dispatchEvent(new Event('editorchange'));
}
function setArguments(value) {
  $('args').value = value;
  $('args').rows = Math.min(8, Math.max(4, value.split('\n').length));
  $('args').scrollTop = $('args').scrollLeft = 0;
}
function displayWorkspace(preferred = {}) {
  const names = [...workspace.keys()].sort();
  const sources = names.filter(name => name.endsWith('.ts'));
  selectOptions($('source-file'), sources, preferred.entry);
  selectOptions($('entry'), sources.filter(name => !name.endsWith('.d.ts')), preferred.entry);
  selectOptions($('wit-file'), names.filter(name => name.endsWith('.wit')), preferred.wit);
  for (const id of ['source-file', 'wit-file', 'entry']) $(id + '-picker').hidden = $(id).options.length < 2;
  setEditor('source', workspace.get($('source-file').value));
  setEditor('wit', workspace.get($('wit-file').value));
  refreshWorlds(preferred.world || '');
}
function clearOutput(message = 'Compile & run to see the result.') {
  artifact = undefined;
  failed = false;
  for (const key of Object.keys(views)) views[key] = '';
  views.result = message;
  $('download-wasm').disabled = $('download-wat').disabled = true;
  $('metrics').textContent = '';
  $('args').removeAttribute('aria-invalid');
  showOutput();
}
function setBusy(busy) {
  for (const id of ['compile', 'run', 'reset', 'close-folder', 'source', 'wit', 'source-file', 'wit-file', 'entry', 'world', 'args']) $(id).disabled = busy || !workspace;
  $('example').disabled = busy || !examples.length;
  $('example').hidden = $('example-label').hidden = snapshot?.kind === 'folder';
  $('open-folder').disabled = busy;
  $('function').disabled = busy || ![...$('function').options].some(option => option.value);
}
function finish() {
  if (operation?.kind === 'compile') { operation.worker.terminate(); clearTimeout(operation.deadline); }
  operation = undefined;
  $('cancel').hidden = true;
  setBusy(false);
}
function loadExample() {
  finish();
  const example = examples.find(example => example.id === $('example').value);
  if (!example) return;
  const files = new Map(example.files.map(([name, contents]) => [name, typeof contents === 'string' ? encode(contents) : new Uint8Array(contents)]));
  snapshot = { kind: 'example', example, files };
  workspace = new Map(files);
  displayWorkspace(example);
  configureProgram(example);
  selectExports(example.exports, example.export);
  $('close-folder').hidden = true;
  clearOutput(); selectTab('result');
  setBusy(false);
  status('Ready to run');
}
function edited(message = 'Edits ready to compile') {
  clearOutput('Your edits have not been compiled yet.');
  status(message);
}
function fail(error, phase = 'compile') {
  const message = typeof error === 'string' ? error : error?.message || String(error);
  const missing = message.match(/WIT export (\S+) needs an exported TypeScript function (\S+)/);
  let title = { arguments: 'Check the arguments', input: 'Choose an interface', compile: 'Could not compile', run: 'Could not run the function', inspect: 'Compiled, but cannot run in this browser', load: 'Could not load the compiler', timeout: 'Run stopped after 30 seconds', folder: 'Could not open this folder' }[phase] || 'Could not complete this operation';
  let explanation = message;
  if (missing) {
    title = 'The WIT interface and TypeScript do not match';
    explanation = `World “${$('world').value}” requires an exported ${missing[2]} function. Add it to the TypeScript source, or choose a world that matches the functions you implemented.`;
  } else if (/Parsing|Parse error/.test(message)) {
    explanation = 'Check the TypeScript syntax.';
  }
  failed = true;
  if (phase === 'arguments') $('args').setAttribute('aria-invalid', 'true');
  views.result = explanation === message ? message : `${explanation}\n\n${message}`;
  views.log += `${message}\n`;
  selectTab('result');
  status(title, 'error');
  finish();
}
function compile(run) {
  if (operation || !workspace) return;
  $('args').removeAttribute('aria-invalid');
  if (!$('world').value) return fail($('world').options.length ? 'Select a WIT world above the WIT source. A world defines which functions this program must implement.' : 'Add a WIT contract to the WIT field, or open the project folder containing it.', 'input');
  let args = [];
  if (run) {
    try { args = JSON.parse($('args').value); if (!Array.isArray(args)) throw new Error(); }
    catch { return fail('Use a JSON array of arguments, such as ["hello"], [21], or []. Object keys and strings need double quotes.', 'arguments'); }
  }
  try {
    const request = compileRequest(workspace, $('entry').value, $('wit-file').value, $('world').value);
    clearOutput('Working…'); selectTab('result');
    status('Loading compiler…', 'busy');
    const worker = new Worker(new URL('./assets/worker.js', import.meta.url), { type: 'module' });
    const running = { kind: 'compile', worker, deadline: setTimeout(() => fail('The operation exceeded 30 seconds. Check for a loop or a slow network request, then try again.', 'timeout'), 30_000) };
    operation = running;
    $('cancel').hidden = false;
    setBusy(true);
    worker.onerror = event => { if (operation === running) fail(event.message || 'Reload the page to retry the compiler download.', 'load'); };
    worker.onmessage = ({ data }) => {
      if (operation !== running) return;
      if (data.kind === 'progress') status(data.message, 'busy');
      if (data.kind === 'compiled') {
        artifact = data;
        $('download-wasm').disabled = false;
        $('metrics').textContent = `${(data.bytes.length / 1024).toFixed(1)} KiB component · ${data.elapsed.toFixed(0)} ms to compile`;
        views.log = data.allocationReport;
      }
      if (data.kind === 'inspection') {
        artifact.wat = data.wat;
        $('download-wat').disabled = false;
        views.wat = data.wat.length > 250_000 ? `${data.wat.slice(0, 250_000)}\n… Preview truncated; download the full WAT.` : data.wat;
        views.interface = data.interface;
        showOutput();
      }
      if (data.kind === 'exports') { selectExports(data.names); $('function').disabled = true; }
      if (data.kind === 'complete') {
        views.result = data.result;
        views.log += data.log;
        showOutput(); finish();
        status(data.needsExport ? 'Choose a function to run' : run ? 'Run complete' : 'Compiled successfully', 'complete');
        if (data.needsExport) $('function').focus({ preventScroll: true });
      }
      if (data.kind === 'error') { views.log += data.log || ''; fail(data.message, data.phase); }
    };
    worker.postMessage({ ...request, run, args, exportName: $('function').value || program?.export });
  } catch (error) { fail(error); }
}

$('example').addEventListener('change', loadExample);
$('run').addEventListener('click', () => compile(true));
$('compile').addEventListener('click', () => compile(false));
$('cancel').addEventListener('click', () => { finish(); views.result = 'Operation cancelled. You can edit the inputs and run again.'; selectTab('result'); status('Cancelled', 'cancelled'); });
$('source').addEventListener('input', () => { workspace.set($('source-file').value, encode($('source').value)); edited(); });
$('wit').addEventListener('input', () => { workspace.set($('wit-file').value, encode($('wit').value)); refreshWorlds(); selectExports([]); edited(); });
$('source-file').addEventListener('change', () => { setEditor('source', workspace.get($('source-file').value)); });
$('wit-file').addEventListener('change', () => { setEditor('wit', workspace.get($('wit-file').value)); refreshWorlds(''); selectExports([]); edited(); });
$('world').addEventListener('change', () => { selectExports([]); edited('Interface changed'); });
$('entry').addEventListener('change', () => {
  const selected = snapshot.programs?.find(program => program.entry === $('entry').value);
  if (selected) displayWorkspace(selected);
  else setEditor('source', workspace.get($('source-file').value = $('entry').value));
  if (selected || program) configureProgram(selected);
  selectExports([]);
  edited('Entrypoint changed');
});
$('function').addEventListener('change', () => {
  if (selectedFunction) savedArguments.set(selectedFunction, $('args').value);
  selectedFunction = $('function').value;
  if (savedArguments.has(selectedFunction)) setArguments(savedArguments.get(selectedFunction));
  functionHelp();
  status('Ready to run');
});
$('args').addEventListener('input', () => { $('args').removeAttribute('aria-invalid'); status('Arguments updated'); });
$('reset').addEventListener('click', () => {
  if (snapshot.kind === 'example') return loadExample();
  workspace = new Map(snapshot.files);
  displayWorkspace(program || { ...snapshot, world: $('world').value });
  if (program) configureProgram(program);
  else savedArguments.clear();
  selectExports([]);
  edited('Folder restored');
});
$('open-folder').addEventListener('click', () => $('folder-input').click());
$('folder-input').addEventListener('change', async event => {
  if (!event.target.files.length) return;
  operation = { kind: 'folder' }; setBusy(true);
  status('Opening folder…', 'busy');
  try {
    const files = await readFolder(event.target.files);
    const programs = packagePrograms(files);
    const names = [...files.keys()].sort();
    const missingWit = !names.some(name => name.endsWith('.wit'));
    if (missingWit) files.set('world.wit', encode(''));
    const preferred = programs[0] || { entry: ['index.ts', 'src/index.ts'].find(name => files.has(name)) || names.find(name => name.endsWith('/index.ts')) || names.find(name => !name.endsWith('.d.ts') && name.endsWith('.ts')), wit: names.find(name => name.endsWith('world.wit')) || 'world.wit' };
    snapshot = { kind: 'folder', files, programs, ...preferred }; workspace = new Map(files);
    displayWorkspace(preferred);
    configureProgram(programs[0]);
    $('close-folder').hidden = false;
    selectExports([]);
    if (missingWit) $('folder-description').textContent = 'No WIT contract was included. Paste it into WIT, or open a parent folder containing the contract.';
    edited('Folder loaded');
  } catch (error) { fail(error, 'folder'); }
  finally { finish(); event.target.value = ''; }
});
$('close-folder').addEventListener('click', loadExample);
for (const button of document.querySelectorAll('[data-tab]')) {
  button.addEventListener('click', () => selectTab(button.dataset.tab));
  button.addEventListener('keydown', event => {
    const tabs = [...document.querySelectorAll('[data-tab]')];
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const index = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (tabs.indexOf(button) + (event.key === 'ArrowLeft' ? -1 : 1) + tabs.length) % tabs.length;
    selectTab(tabs[index].dataset.tab); tabs[index].focus();
  });
}
for (const extension of ['wasm', 'wat']) $('download-' + extension).addEventListener('click', () => {
  const url = URL.createObjectURL(new Blob([extension === 'wasm' ? artifact.bytes : artifact.wat], { type: extension === 'wasm' ? 'application/wasm' : 'text/plain' }));
  const link = document.createElement('a'); link.href = url; link.download = `component.${extension}`; link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});
document.addEventListener('keydown', event => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') { event.preventDefault(); compile(true); }
});
let examples = [];
setBusy(true);
try {
  const response = await fetch(new URL('./examples.json', import.meta.url));
  if (!response.ok) throw new Error('Could not load examples. Reload the page or open a local folder.');
  examples = await response.json();
  selectOptions($('example'), examples.map(example => example.id));
  for (const option of $('example').options) option.textContent = examples.find(example => example.id === option.value).title;
  loadExample();
} catch (error) { fail(error, 'load'); }
