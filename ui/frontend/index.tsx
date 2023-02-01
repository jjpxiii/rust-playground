import 'core-js';
import 'regenerator-runtime/runtime';

import 'normalize.css/normalize.css';
import './index.module.css';

import React from 'react';
import { createRoot } from 'react-dom/client';
import { Provider } from 'react-redux';

import {
  editCode,
  enableFeatureGate,
  gotoPosition,
  selectText,
  addImport,
  performCratesLoad,
  performVersionsLoad,
  reExecuteWithBacktrace,
  browserWidthChanged,
} from './actions';
import { configureRustErrors } from './highlighting';
import PageSwitcher from './PageSwitcher';
import playgroundApp from './reducers';
import Router from './Router';
import configureStore from './configureStore';
import openWebSocket from './websocket';

// Might be null;
const socket = openWebSocket(window.location);

const store = configureStore(window);

const z = (evt: MediaQueryList | MediaQueryListEvent) => { store.dispatch(browserWidthChanged(evt.matches)); };

const maxWidthMediaQuery = window.matchMedia('(max-width: 1600px)');
z(maxWidthMediaQuery);
maxWidthMediaQuery.addListener(z);

configureRustErrors({
  enableFeatureGate: featureGate => store.dispatch(enableFeatureGate(featureGate)),
  gotoPosition: (line, col) => store.dispatch(gotoPosition(line, col)),
  selectText: (start, end) => store.dispatch(selectText(start, end)),
  addImport: (code) => store.dispatch(addImport(code)),
  reExecuteWithBacktrace: () => store.dispatch(reExecuteWithBacktrace()),
  getChannel: () => store.getState().configuration.channel,
});

store.dispatch(performCratesLoad());
store.dispatch(performVersionsLoad());

window.rustPlayground = {
  setCode: code => {
    store.dispatch(editCode(code));
  },
  // Temporarily storing this as a global to prevent it from being
  // garbage collected (at least by Safari).
  webSocket: socket,
};

const container = document.getElementById('playground');
if (container) {
  const root = createRoot(container);
  root.render(
    <Provider store={store}>
      <Router store={store} reducer={playgroundApp}>
        <PageSwitcher />
      </Router>
    </Provider>,
  );
}
