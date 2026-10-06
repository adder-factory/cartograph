import router from '@ohos.router';

@Component
export struct CounterView {
  @State count: number = 0;
  @Prop title: string = '';
  @Link total: number;

  build() {
    Column() {
      Text(this.title)
      Button('Tap').onClick(() => {
        this.increment();
        router.pushUrl({ url: 'pages/Next' });
      })
    }
  }

  increment(): void {
    this.count += 1;
    this.total += 1;
  }
}

@Builder
function FooterBuilder(text: string) {
  Text(text)
}

class WorkerService {
  run(): void {
    new Worker().start();
  }
}

class Worker {
  start(): void {}
}
