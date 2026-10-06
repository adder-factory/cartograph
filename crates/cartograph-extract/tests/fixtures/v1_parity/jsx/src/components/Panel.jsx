import React, { Component } from 'react';
import { Card } from './Card.jsx';
import Base, { Mixin } from '../widgets/widgets.jsx';

export class Panel extends Component {
  state = { open: false };
  toggle = () => {
    this.setState({ open: !this.state.open });
    logToggle(this.state.open);
  };
  handleResize = debounce(() => {
    this.measure();
  }, 50);

  measure() {
    return this.props.width;
  }

  render() {
    return <Card title="panel">{this.state.open && <p>open</p>}</Card>;
  }
}

export class FancyPanel extends Panel {}

export class LegacyWidget extends Base {}

export class MixedWidget extends Mixin.Inner {}

function logToggle(value) {
  console.log(value);
}

function debounce(fn, wait) {
  return fn;
}

export default class PanelHost extends React.PureComponent {
  render() {
    return <Panel width={100} />;
  }
}
