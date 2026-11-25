/** Vite 把 import 进来的字体文件打包进去，给出它的地址 */
declare module "*.woff2" {
  const url: string;
  export default url;
}
