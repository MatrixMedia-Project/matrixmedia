/* @refresh reload */
import { render } from 'solid-js/web';
import { App } from './App';
import './styles/tokens.css';
import './styles/widget.css';

const root = document.getElementById('root');

if (!root) {
  throw new Error('Root element #root not found');
}

render(() => <App />, root);
