import React, { useState, useEffect, createContext, useContext } from 'react';
import PropTypes from 'prop-types';
import * as widgets from '../widgets/widgets.jsx';
import { Badge, formatTitle } from '../widgets/widgets.jsx';

export const SettingsContext = createContext({ dense: false });

export function useFetch(url) {
  const [data, setData] = useState(null);
  useEffect(() => {
    fetchJson(url).then(setData);
  }, [url]);
  return data;
}

function fetchJson(url) {
  return Promise.resolve({ url });
}

export function Card({ title, children }) {
  const settings = useContext(SettingsContext);
  const data = useFetch('/api/card');
  return (
    <div className={settings.dense ? 'dense' : 'roomy'}>
      <widgets.Header text={formatTitle(title)} />
      <Badge count={data ? 1 : 0} />
      <span>{children}</span>
    </div>
  );
}

Card.propTypes = {
  title: PropTypes.string,
};

export const CardList = ({ items }) => (
  <ul>
    {items.map((item) => (
      <Card key={item.id} title={item.title} />
    ))}
  </ul>
);

export default function Dashboard() {
  return (
    <SettingsContext.Provider value={{ dense: true }}>
      <CardList items={[]} />
    </SettingsContext.Provider>
  );
}
