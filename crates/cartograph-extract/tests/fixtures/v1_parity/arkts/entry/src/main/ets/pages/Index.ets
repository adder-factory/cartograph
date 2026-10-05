import { Item } from '../model/Item';
import { Color, arrow, PI_APPROX } from '../common/Utils';
import * as Utils from '../common/Utils';
import { CounterView } from '../components/CounterView';

@Entry
@Component
struct Index {
  @State items: Item[] = [];
  @State total: number = 0;
  private color: Color = Color.Red;

  aboutToAppear(): void {
    this.items.push(Item.create(1));
    const doubled = arrow(PI_APPROX);
    Utils.helper(doubled);
  }

  build() {
    Column() {
      CounterView({ title: 'Count', total: $total })
      ForEach(this.items, (item: Item) => {
        Text(item.label())
      })
    }
  }
}
