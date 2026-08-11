#include <fstream>
int main(int argc, char** argv) {
  long long output = 0, answer = 0;
  std::ifstream(argv[2]) >> output;
  std::ifstream(argv[3]) >> answer;
  return output == answer ? 0 : 1;
}
