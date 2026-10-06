import IconButton, { Button, ThemeContext } from '../src/components/Button';

export async function getServerSideProps() {
  return { props: { title: 'Home' } };
}

export default function HomePage({ title }: { title: string }) {
  return (
    <ThemeContext.Provider value="ghost">
      <Button label={title} />
      <IconButton label="icon" />
    </ThemeContext.Provider>
  );
}
