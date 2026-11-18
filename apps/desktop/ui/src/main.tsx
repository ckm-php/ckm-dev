import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { getBridge } from './bridge';
import './app.css';

const rootElement = document.getElementById('root');
if (!rootElement) {
  throw new Error('index.html has no #root element');
}

const { bridge, isMock } = await getBridge();

createRoot(rootElement).render(
  <StrictMode>
    <App bridge={bridge} isMock={isMock} />
  </StrictMode>,
);
